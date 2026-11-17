unit WarnAllDefault;

interface

procedure DeprecatedRoutine; deprecated;

{$WARN ALL OFF}

type
  TObject = class
  end;

  TBaseFieldOff = class(TObject)
  protected
    ValueOff: Integer;
  end;

  TChildFieldOff = class(TBaseFieldOff)
  private
    FChildValueOff: Integer;
  public
    property ValueOff: Integer read FChildValueOff;
  end;

  TBaseVirtualOff = class(TObject)
  public
    procedure InvokeOff; virtual;
  end;

  TChildVirtualOff = class(TBaseVirtualOff)
  public
    procedure InvokeOff;
  end;

  TUnusedField = class(TObject)
  private
    FUnused: Integer;
  end;

{$WARN ALL DEFAULT}

type
  TBaseFieldDefault = class(TObject)
  protected
    ValueDefault: Integer;
  end;

  TChildFieldDefault = class(TBaseFieldDefault)
  private
    FChildValueDefault: Integer;
  public
    property ValueDefault: Integer read FChildValueDefault;
  end;

  TBaseVirtualDefault = class(TObject)
  public
    procedure InvokeDefault; virtual;
  end;

  TChildVirtualDefault = class(TBaseVirtualDefault)
  public
    procedure InvokeDefault;
  end;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure ExerciseWarnings;
begin
  {$WARN ALL OFF}
  DeprecatedRoutine;
  {$WARN ALL DEFAULT}
  DeprecatedRoutine;
end;

procedure TBaseVirtualOff.InvokeOff;
begin
end;

procedure TChildVirtualOff.InvokeOff;
begin
end;

procedure TBaseVirtualDefault.InvokeDefault;
begin
end;

procedure TChildVirtualDefault.InvokeDefault;
begin
end;

end.
