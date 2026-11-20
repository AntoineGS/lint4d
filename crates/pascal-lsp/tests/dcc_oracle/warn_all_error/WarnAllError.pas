unit WarnAllError;

interface

{$WARN ALL ERROR}

procedure DeprecatedRoutine; deprecated;

type
  TObject = class
  end;

  TBaseField = class(TObject)
  protected
    Value: Integer;
  end;

  TChildField = class(TBaseField)
  private
    FChildValue: Integer;
  public
    property Value: Integer read FChildValue;
  end;

  TBaseVirtual = class(TObject)
  public
    procedure Invoke; virtual;
  end;

  TChildVirtual = class(TBaseVirtual)
  public
    procedure Invoke;
  end;

  TUnusedField = class(TObject)
  private
    FUnused: Integer;
  end;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure ExerciseWarnings;
begin
  DeprecatedRoutine;
end;

procedure TBaseVirtual.Invoke;
begin
end;

procedure TChildVirtual.Invoke;
begin
end;

end.
