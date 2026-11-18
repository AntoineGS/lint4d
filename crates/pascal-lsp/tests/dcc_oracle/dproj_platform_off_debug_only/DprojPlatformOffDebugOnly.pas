unit DprojPlatformOffDebugOnly;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

  TChild = class(TBase)
  private
    FChild: Integer;
  public
    property Value: Integer read FChild;
  end;

implementation

end.
