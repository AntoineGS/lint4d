unit SwitchWarnIdError;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  public
    procedure Invoke; virtual;
  end;

{$WARN HIDING_MEMBER ERROR}
{$WARN HIDDEN_VIRTUAL ERROR}
  TChild = class(TBase)
  private
    FChild: Integer;
  public
    property Value: Integer read FChild;
    procedure Invoke;
  end;

implementation

procedure TBase.Invoke;
begin
end;

procedure TChild.Invoke;
begin
end;

end.
